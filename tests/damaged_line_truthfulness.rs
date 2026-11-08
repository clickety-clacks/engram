//! Damaged transcript lines: ingest reads past them, records each as a gap,
//! and every answer that depends on the file reports it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};

const P1: &str = "amberfinch before damage";
const P2: &str = "cobaltwren inside damage";
const P3: &str = "dovetailowl after damage";
const P4: &str = "emberlynx pending tail";

fn record(second: u32, text: &str) -> String {
    format!(
        "{{\"timestamp\":\"2026-03-01T00:00:{second:02}Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{text}\"}}]}}}}"
    )
}

/// A record cut off mid-string, after `phrase` and before its closing quote.
fn cut_record(second: u32, phrase: &str) -> String {
    let full = record(second, &format!("{phrase} and the rest"));
    let end = full.find(phrase).expect("phrase") + phrase.len() + 4;
    full[..end].to_string()
}

/// Fixture F: section 5 of the damaged-line spec, built from synthetic records.
struct Fixture {
    bytes: Vec<u8>,
    c1_offset: u64,
    damaged_bytes: u64,
    pending_offset: u64,
    pending_rest: Vec<u8>,
    undamaged_prefix: Vec<u8>,
    c1: Vec<u8>,
}

fn fixture_f() -> Fixture {
    let mut bytes = Vec::new();
    let line = |bytes: &mut Vec<u8>, text: &str| {
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(b'\n');
    };
    line(
        &mut bytes,
        "{\"timestamp\":\"2026-03-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"damaged-fixture\",\"cwd\":\"/repo\"}}",
    );
    line(&mut bytes, &record(1, "opening words"));
    line(&mut bytes, &record(2, P1));
    let undamaged_prefix = bytes.clone();

    let c1_offset = bytes.len() as u64;
    let c1 = format!("{}\n", cut_record(3, P2)).into_bytes();
    bytes.extend_from_slice(&c1);
    line(&mut bytes, &record(4, "between first and second damage"));

    let c2 = format!("{}{}\n", cut_record(5, "cut short"), record(6, "glued on"));
    bytes.extend_from_slice(c2.as_bytes());
    line(&mut bytes, &record(7, "after second damage"));
    line(&mut bytes, "  ");
    line(&mut bytes, &record(8, "after the blank line"));

    let mut c3 = b"\0\0\0".to_vec();
    c3.extend_from_slice(record(9, "behind nul bytes").as_bytes());
    c3.push(b'\n');
    bytes.extend_from_slice(&c3);
    line(&mut bytes, &record(10, "after third damage"));
    line(&mut bytes, &record(11, P3));

    let pending_offset = bytes.len() as u64;
    let pending = record(12, &format!("{P4} completes later"));
    let split = pending.find(P4).expect("p4") + P4.len() + 3;
    bytes.extend_from_slice(&pending.as_bytes()[..split]);
    let mut pending_rest = pending.as_bytes()[split..].to_vec();
    pending_rest.push(b'\n');

    Fixture {
        bytes,
        c1_offset,
        damaged_bytes: (c1.len() + c2.len() + c3.len()) as u64,
        pending_offset,
        pending_rest,
        undamaged_prefix,
        c1,
    }
}

fn engram(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    fs::create_dir_all(home).expect("home dir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engram"));
    cmd.current_dir(cwd).args(args);
    if cfg!(windows) {
        cmd.env("USERPROFILE", home).env_remove("HOME");
    } else {
        cmd.env("HOME", home);
    }
    cmd.output().expect("engram runs")
}

fn engram_json(cwd: &Path, home: &Path, args: &[&str]) -> Value {
    let output = engram(cwd, home, args);
    assert!(
        output.status.success(),
        "engram {args:?} failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json stdout")
}

fn path_identity(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonical path")
        .to_string_lossy()
        .into_owned()
}

fn cursor(repo: &Path, source: &Path) -> u64 {
    let key = hex(&Sha256::digest(path_identity(source).as_bytes()));
    let path = repo.join(".engram/cursors").join(format!("{key}.json"));
    let state: Value =
        serde_json::from_str(&fs::read_to_string(path).expect("cursor file")).expect("cursor json");
    state["byte_cursor"].as_u64().expect("byte_cursor")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn append(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open source")
        .write_all(bytes)
        .expect("append");
}

fn found(repo: &Path, home: &Path, phrase: &str) -> bool {
    let output = engram(repo, home, &["grep", phrase]);
    String::from_utf8_lossy(&output.stdout).contains(phrase)
}

fn recorded_gap_lines(home: &Path) -> i64 {
    let db = rusqlite::Connection::open(home.join(".engram/index.sqlite")).expect("index");
    db.query_row("SELECT COUNT(*) FROM source_gaps", [], |row| row.get(0))
        .expect("gap rows")
}

struct Store {
    _temp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    source: PathBuf,
}

fn store() -> Store {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo");
    let source = repo.join("damaged.codex.jsonl");
    Store {
        _temp: temp,
        home,
        repo,
        source,
    }
}

fn assert_gap(output: &Value, source: &Path, fixture: &Fixture, lines: u64) {
    assert_eq!(output["status"], "partial", "{output}");
    let gaps = output["gaps"].as_array().expect("gaps array");
    assert_eq!(gaps.len(), 1, "{output}");
    assert_eq!(gaps[0]["path"], path_identity(source));
    assert_eq!(gaps[0]["damaged_lines"], lines);
    assert_eq!(gaps[0]["first_damaged_offset"], fixture.c1_offset);
    if lines == 3 {
        assert_eq!(gaps[0]["damaged_bytes"], fixture.damaged_bytes);
    }
}

#[test]
fn ingest_reads_past_damaged_lines_and_reports_the_gap() {
    let fixture = fixture_f();
    let s = store();
    fs::write(&s.source, &fixture.bytes).expect("fixture");

    let first = engram_json(&s.repo, &s.home, &["ingest"]);
    assert_gap(&first, &s.source, &fixture, 3);
    assert_eq!(first["failure_count"], 0);
    assert_eq!(cursor(&s.repo, &s.source), fixture.pending_offset);
    assert!(found(&s.repo, &s.home, P1));
    assert!(found(&s.repo, &s.home, P3));
    assert!(!found(&s.repo, &s.home, P2));
    assert!(!found(&s.repo, &s.home, P4));
    // A gap event is never a grep hit, even on its own source path.
    assert!(!found(&s.repo, &s.home, "damaged.codex.jsonl"));

    // Exactly once: a rerun with no new bytes imports nothing and keeps the entry.
    let second = engram_json(&s.repo, &s.home, &["ingest"]);
    assert_gap(&second, &s.source, &fixture, 3);
    assert_eq!(second["imported_tapes"], 0);

    // Completing the pending tail indexes it; the gap entry is unchanged.
    append(&s.source, &fixture.pending_rest);
    let third = engram_json(&s.repo, &s.home, &["ingest"]);
    assert_gap(&third, &s.source, &fixture, 3);
    assert_eq!(third["imported_tapes"], 1);
    assert!(found(&s.repo, &s.home, P4));
    assert_eq!(
        cursor(&s.repo, &s.source),
        fs::metadata(&s.source).expect("source").len()
    );

    // An undamaged file in the same run is not reported.
    let clean = s.repo.join("clean.codex.jsonl");
    fs::write(&clean, &fixture.undamaged_prefix).expect("clean");
    let clean_only = engram_json(
        &s.repo,
        &s.home,
        &["ingest", clean.to_str().expect("utf8 path")],
    );
    assert_eq!(clean_only["status"], "ok", "{clean_only}");
    assert!(clean_only.get("gaps").is_none());
}

#[test]
fn gap_only_append_writes_a_gap_tape() {
    let fixture = fixture_f();
    let s = store();
    fs::write(&s.source, &fixture.undamaged_prefix).expect("prefix");
    let first = engram_json(&s.repo, &s.home, &["ingest"]);
    assert_eq!(first["status"], "ok", "{first}");

    append(&s.source, &fixture.c1);
    let second = engram_json(&s.repo, &s.home, &["ingest"]);
    assert_gap(&second, &s.source, &fixture, 1);
    assert_eq!(second["imported_tapes"], 1);
    assert_eq!(
        cursor(&s.repo, &s.source),
        fixture.c1_offset + fixture.c1.len() as u64
    );
}

#[cfg(unix)]
#[test]
fn gap_is_recorded_before_the_cursor_moves() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixture_f();
    let s = store();
    fs::write(&s.source, &fixture.undamaged_prefix).expect("prefix");
    engram_json(&s.repo, &s.home, &["ingest"]);
    let before = cursor(&s.repo, &s.source);
    append(&s.source, &fixture.bytes[fixture.undamaged_prefix.len()..]);

    let tapes = s.home.join(".engram/tapes");
    let set_mode = |mode| {
        let mut permissions = fs::metadata(&tapes).expect("tapes dir").permissions();
        permissions.set_mode(mode);
        fs::set_permissions(&tapes, permissions).expect("chmod");
    };
    set_mode(0o555);
    let failed = engram(&s.repo, &s.home, &["ingest"]);
    set_mode(0o755);
    assert!(!failed.status.success(), "tape write should fail");
    assert_eq!(cursor(&s.repo, &s.source), before);
    assert_eq!(recorded_gap_lines(&s.home), 0);

    let retried = engram_json(&s.repo, &s.home, &["ingest"]);
    assert_gap(&retried, &s.source, &fixture, 3);
    assert_eq!(cursor(&s.repo, &s.source), fixture.pending_offset);
}

#[cfg(unix)]
fn write_export_topology(home: &Path, self_label: &str, db: &Path, tapes: &Path) {
    let engram_dir = home.join(".engram");
    fs::create_dir_all(&engram_dir).expect("engram dir");
    fs::write(
        engram_dir.join("topology.yml"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "self": self_label,
            "exports": {"default": {"db": db, "tape_dirs": [tapes]}},
        }))
        .expect("topology json"),
    )
    .expect("write topology");
}

#[cfg(unix)]
fn store_db(home: &Path) -> PathBuf {
    home.join(".engram/index.sqlite")
}

#[cfg(unix)]
fn store_tapes(home: &Path) -> PathBuf {
    home.join(".engram/tapes")
}

#[cfg(unix)]
fn export_row(status: &Value) -> &Value {
    let rows = status["local_exports"].as_array().expect("local_exports");
    assert_eq!(rows.len(), 1, "{status}");
    &rows[0]
}

#[cfg(unix)]
fn assert_status_gap(status: &Value, source: &Path) {
    assert_eq!(status["status"], "partial", "{status}");
    let row = export_row(status);
    assert_eq!(row["status"], "partial", "{status}");
    let gaps = row["gaps"].as_array().expect("row gaps");
    assert_eq!(gaps.len(), 1, "{status}");
    assert_eq!(gaps[0]["path"], path_identity(source));
    assert_eq!(gaps[0]["damaged_lines"], 3);
}

// Topology status is unsupported on Windows.
#[cfg(unix)]
#[test]
fn local_status_reports_the_gap_and_survives_a_rebuild() {
    let fixture = fixture_f();
    let s = store();
    fs::write(&s.source, &fixture.bytes).expect("fixture");
    engram_json(&s.repo, &s.home, &["ingest"]);
    write_export_topology(&s.home, "owner", &store_db(&s.home), &store_tapes(&s.home));

    let status = engram_json(&s.repo, &s.home, &["topology", "status", "--check-exports"]);
    assert_status_gap(&status, &s.source);

    // An undamaged store stays ok.
    let clean = store();
    fs::write(&clean.source, &fixture.undamaged_prefix).expect("clean");
    engram_json(&clean.repo, &clean.home, &["ingest"]);
    write_export_topology(
        &clean.home,
        "owner",
        &store_db(&clean.home),
        &store_tapes(&clean.home),
    );
    let clean_status = engram_json(
        &clean.repo,
        &clean.home,
        &["topology", "status", "--check-exports"],
    );
    assert_eq!(clean_status["status"], "ok", "{clean_status}");
    assert_eq!(export_row(&clean_status)["status"], "ok");
    assert!(export_row(&clean_status).get("gaps").is_none());

    // Rebuild a staging index from the tapes alone; it reports the same gap.
    let staging = s.repo.parent().expect("temp root").join("staging");
    let staging_engram = staging.join(".engram");
    fs::create_dir_all(staging_engram.join("tapes")).expect("staging tapes");
    for entry in fs::read_dir(store_tapes(&s.home)).expect("tapes") {
        let entry = entry.expect("tape entry");
        fs::copy(
            entry.path(),
            staging_engram.join("tapes").join(entry.file_name()),
        )
        .expect("copy tape");
    }
    let staging_db = staging.join("index.sqlite");
    let staging_home = staging.join("home");
    fs::create_dir_all(staging_home.join(".engram")).expect("staging home");
    fs::write(
        staging_home.join(".engram/config.yml"),
        format!(
            "db: {}\ntapes_dir: {}\n",
            staging_db.display(),
            staging_engram.join("tapes").display()
        ),
    )
    .expect("staging config");
    let rebuilt = engram_json(&staging, &staging_home, &["fingerprint"]);
    assert_eq!(rebuilt["failure_count"], 0, "{rebuilt}");
    write_export_topology(
        &staging_home,
        "staging",
        &staging_db,
        &staging_engram.join("tapes"),
    );
    let staging_status = engram_json(
        &staging,
        &staging_home,
        &["topology", "status", "--check-exports"],
    );
    assert_status_gap(&staging_status, &s.source);
}

// Topology status is unsupported on Windows.
#[cfg(unix)]
#[test]
fn an_index_without_the_gap_table_reads_as_no_gaps() {
    let fixture = fixture_f();
    let s = store();
    fs::write(&s.source, &fixture.bytes).expect("fixture");
    engram_json(&s.repo, &s.home, &["ingest"]);
    write_export_topology(&s.home, "owner", &store_db(&s.home), &store_tapes(&s.home));

    // An index written before gap recording has schema v4 and no gap table.
    let db = rusqlite::Connection::open(store_db(&s.home)).expect("index");
    db.execute_batch("DROP TABLE source_gaps")
        .expect("drop table");
    drop(db);
    let status = engram_json(&s.repo, &s.home, &["topology", "status", "--check-exports"]);
    assert_eq!(status["status"], "ok", "{status}");
    assert_eq!(export_row(&status)["status"], "ok");

    // A writer opening that index adds the table without changing the schema version.
    let other = s.repo.join("other.codex.jsonl");
    fs::write(&other, &fixture.undamaged_prefix).expect("other");
    engram_json(&s.repo, &s.home, &["ingest", other.to_str().expect("utf8")]);
    let db = rusqlite::Connection::open(store_db(&s.home)).expect("index");
    let tables: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'source_gaps'",
            [],
            |row| row.get(0),
        )
        .expect("table count");
    assert_eq!(tables, 1);
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version");
    assert_eq!(version, 4);
}

#[cfg(unix)]
mod peers {
    use super::*;

    /// An owner home serving one store as export `default`, and a searcher
    /// home that peers with it over a local `peer-serve` command.
    pub(super) struct Pair {
        pub(super) owner: Store,
        pub(super) searcher: Store,
    }

    pub(super) fn pair(owner_bin: &str, owner_bytes: &[u8]) -> Pair {
        let owner = store();
        fs::write(&owner.source, owner_bytes).expect("owner fixture");
        let ingest = Command::new(owner_bin)
            .current_dir(&owner.repo)
            .env("HOME", {
                fs::create_dir_all(&owner.home).expect("owner home");
                &owner.home
            })
            .arg("ingest")
            .output()
            .expect("owner ingest");
        assert!(
            ingest.status.success(),
            "{}",
            String::from_utf8_lossy(&ingest.stderr)
        );
        write_export_topology(
            &owner.home,
            "owner",
            &store_db(&owner.home),
            &store_tapes(&owner.home),
        );
        // The searcher has its own undamaged store; local queries need one.
        let searcher = store();
        let clean = searcher.repo.join("clean.codex.jsonl");
        fs::write(&clean, &fixture_f().undamaged_prefix).expect("searcher store");
        engram_json(&searcher.repo, &searcher.home, &["ingest"]);
        let searcher_engram = searcher.home.join(".engram");
        fs::create_dir_all(&searcher_engram).expect("searcher engram");
        fs::write(
            searcher_engram.join("topology.yml"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "self": "searcher",
                "peers": {"owner": {
                    "command": [
                        "/usr/bin/env",
                        format!("HOME={}", owner.home.display()),
                        owner_bin,
                        "peer-serve",
                        "--stdio",
                    ],
                    "engram": owner_bin,
                    "exports": ["default"],
                }},
            }))
            .expect("searcher topology"),
        )
        .expect("write searcher topology");
        Pair { owner, searcher }
    }

    pub(super) fn run(bin: &str, s: &Store, args: &[&str]) -> (bool, Value, String) {
        fs::create_dir_all(&s.home).expect("home");
        let output = Command::new(bin)
            .current_dir(&s.repo)
            .env("HOME", &s.home)
            .args(args)
            .output()
            .expect("engram runs");
        let stdout: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        (
            output.status.success(),
            stdout,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    pub(super) fn source<'a>(answer: &'a Value, store: &str) -> &'a Value {
        answer["federation"]["sources"]
            .as_array()
            .expect("sources")
            .iter()
            .find(|row| row["store"] == store)
            .unwrap_or_else(|| panic!("no source {store} in {answer}"))
    }

    pub(super) fn assert_damaged_source(answer: &Value, store: &str, path: &Path) {
        assert_eq!(answer["federation"]["coverage"], "partial", "{answer}");
        let row = source(answer, store);
        assert_eq!(row["status"], "partial", "{answer}");
        let failures = row["failures"].as_array().expect("failures");
        assert!(
            failures
                .iter()
                .any(|failure| failure["error"]["code"] == "damaged_line"
                    && failure["path"] == path_identity(path)),
            "{answer}"
        );
    }

    pub(super) fn has_hit(answer: &Value) -> bool {
        answer["sessions"]
            .as_array()
            .is_some_and(|sessions| !sessions.is_empty())
    }

    const FIXED: &str = env!("CARGO_BIN_EXE_engram");

    #[test]
    fn federated_answers_report_the_gap() {
        let fixture = fixture_f();
        let p = pair(FIXED, &fixture.bytes);

        let (_, status, stderr) = run(
            FIXED,
            &p.searcher,
            &["topology", "status", "--peers", "owner"],
        );
        assert_eq!(status["status"], "partial", "{status} {stderr}");
        let machine = &status["peers"][0];
        assert_eq!(machine["status"], "partial", "{status}");
        let export = &machine["exports"][0];
        assert_eq!(export["status"], "partial", "{status}");
        assert_eq!(export["gaps"][0]["path"], path_identity(&p.owner.source));
        assert_eq!(export["gaps"][0]["damaged_lines"], 3);

        for phrase in [P1, P3] {
            let (ok, answer, stderr) =
                run(FIXED, &p.searcher, &["grep", phrase, "--peers", "owner"]);
            assert!(ok, "{stderr}");
            assert!(has_hit(&answer), "{answer}");
            assert_damaged_source(&answer, "owner/default", &p.owner.source);
        }

        let (ok, answer, stderr) = run(FIXED, &p.searcher, &["grep", P2, "--peers", "owner"]);
        assert!(!ok);
        assert!(stderr.contains("no_results"), "{stderr}");
        assert_damaged_source(&answer, "owner/default", &p.owner.source);

        let (ok, _, stderr) = run(
            FIXED,
            &p.searcher,
            &["grep", P1, "--peers", "owner", "--require-complete"],
        );
        assert!(!ok);
        assert!(stderr.contains("incomplete_coverage"), "{stderr}");

        let target = p.searcher.repo.join("notes.txt");
        fs::write(&target, "one line\n").expect("explain target");
        let range = format!("{}:1-1", target.display());
        let (_, answer, stderr) = run(FIXED, &p.searcher, &["explain", &range, "--peers", "owner"]);
        assert_damaged_source(&answer, "owner/default", &p.owner.source);
        assert!(!stderr.contains("protocol_error"), "{stderr}");
    }

    #[test]
    fn the_searchers_own_damaged_store_is_partial_too() {
        let fixture = fixture_f();
        let p = pair(FIXED, &fixture.undamaged_prefix);
        fs::write(&p.searcher.source, &fixture.bytes).expect("searcher fixture");
        engram_json(&p.searcher.repo, &p.searcher.home, &["ingest"]);

        let (ok, answer, stderr) = run(FIXED, &p.searcher, &["grep", P1, "--peers", "owner"]);
        assert!(ok, "{stderr}");
        assert!(has_hit(&answer), "{answer}");
        assert_damaged_source(&answer, "searcher/local:0", &p.searcher.source);
        assert_eq!(source(&answer, "owner/default")["status"], "ok", "{answer}");

        let (ok, answer, stderr) = run(FIXED, &p.searcher, &["grep", P2, "--peers", "owner"]);
        assert!(!ok);
        assert!(stderr.contains("no_results"), "{stderr}");
        assert_damaged_source(&answer, "searcher/local:0", &p.searcher.source);

        let target = p.searcher.repo.join("notes.txt");
        fs::write(&target, "one line\n").expect("explain target");
        let range = format!("{}:1-1", target.display());
        let (_, answer, _) = run(FIXED, &p.searcher, &["explain", &range, "--peers", "owner"]);
        assert_damaged_source(&answer, "searcher/local:0", &p.searcher.source);
    }
}

/// Checks against the published v0.2.12 binary. They run when
/// `ENGRAM_V0212_BIN` names that binary and are skipped otherwise.
#[cfg(unix)]
mod v0212 {
    use super::peers::{assert_damaged_source, has_hit, pair, run, source};
    use super::*;

    const FIXED: &str = env!("CARGO_BIN_EXE_engram");

    fn old_bin() -> Option<String> {
        let bin = std::env::var("ENGRAM_V0212_BIN").ok();
        if bin.is_none() {
            eprintln!("ENGRAM_V0212_BIN is not set; skipping the v0.2.12 checks");
        }
        bin
    }

    fn tape_ids(home: &Path) -> std::collections::BTreeSet<String> {
        fs::read_dir(store_tapes(home))
            .map(|entries| {
                entries
                    .map(|entry| {
                        entry
                            .expect("tape")
                            .file_name()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn an_older_searcher_sees_a_fixed_owners_gap() {
        let Some(old) = old_bin() else { return };
        let fixture = fixture_f();
        let p = pair(FIXED, &fixture.bytes);

        let (_, status, stderr) = run(
            &old,
            &p.searcher,
            &["topology", "status", "--peers", "owner"],
        );
        assert!(
            !status.to_string().contains("protocol_error"),
            "{status} {stderr}"
        );
        assert_eq!(status["peers"][0]["exports"][0]["status"], "ok", "{status}");

        let (ok, answer, stderr) = run(&old, &p.searcher, &["grep", P1, "--peers", "owner"]);
        assert!(ok, "{stderr}");
        assert!(has_hit(&answer), "{answer}");
        assert_damaged_source(&answer, "owner/default", &p.owner.source);

        // An older status check on a fixed index holding the gap reads it as ok.
        let (_, status, stderr) = run(&old, &p.owner, &["topology", "status", "--check-exports"]);
        assert_eq!(status["status"], "ok", "{status} {stderr}");
    }

    #[test]
    fn a_fixed_searcher_greps_an_older_owner() {
        let Some(old) = old_bin() else { return };
        let fixture = fixture_f();
        let p = pair(&old, &fixture.bytes);
        let (ok, answer, stderr) = run(FIXED, &p.searcher, &["grep", P1, "--peers", "owner"]);
        assert!(ok, "{stderr}");
        assert!(has_hit(&answer), "{answer}");
        assert_eq!(source(&answer, "owner/default")["status"], "ok", "{answer}");
        assert!(!answer.to_string().contains("incompatible"), "{answer}");
    }

    #[test]
    fn a_file_stuck_under_the_older_build_resumes() {
        let Some(old) = old_bin() else { return };
        let fixture = fixture_f();
        let s = store();
        fs::write(&s.source, &fixture.bytes).expect("fixture");
        let (ok, _, stderr) = run(&old, &s, &["ingest"]);
        assert!(ok, "{stderr}");
        assert_eq!(cursor(&s.repo, &s.source), fixture.c1_offset);
        let before = tape_ids(&s.home);

        let resumed = engram_json(
            &s.repo,
            &s.home,
            &["ingest", s.source.to_str().expect("utf8")],
        );
        assert_gap(&resumed, &s.source, &fixture, 3);
        assert!(found(&s.repo, &s.home, P3));
        let after = tape_ids(&s.home);
        assert!(before.is_subset(&after));
        assert_eq!(
            resumed["imported_tapes"].as_u64().expect("imported"),
            (after.len() - before.len()) as u64,
            "{resumed}"
        );
    }

    #[test]
    fn undamaged_adapter_fixtures_produce_the_same_tapes() {
        let Some(old) = old_bin() else { return };
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut ids = Vec::new();
        for (bin, home) in [(old.as_str(), "old-home"), (FIXED, "fixed-home")] {
            let _ = fs::remove_dir_all(&repo);
            copy_tree(&fixtures, &repo);
            let s = Store {
                _temp: tempfile::tempdir().expect("unused"),
                home: temp.path().join(home),
                repo: repo.clone(),
                source: repo.clone(),
            };
            let (ok, output, stderr) = run(bin, &s, &["ingest"]);
            assert!(ok, "{output} {stderr}");
            ids.push(tape_ids(&s.home));
        }
        assert!(!ids[0].is_empty());
        assert_eq!(ids[0], ids[1]);
    }

    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("dir");
        for entry in fs::read_dir(from).expect("read dir") {
            let entry = entry.expect("entry");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("type").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).expect("copy");
            }
        }
    }
}

struct Watch(std::process::Child);

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(what: &str, log: &Path, mut check: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !check() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}; watch log:\n{}\nwatch stderr:\n{}",
            fs::read_to_string(log).unwrap_or_default(),
            fs::read_to_string(log.with_file_name("watch.stderr")).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn partial_logged(log: &Path, file: &str) -> bool {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .any(|line| {
            line.contains("ingest partial path=")
                && line.contains(file)
                && line.ends_with("damaged_lines=3")
        })
}

#[test]
fn the_watcher_catches_up_stuck_and_new_damaged_files() {
    let fixture = fixture_f();
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let sessions = home.join("sessions");
    let store = temp.path().join("store");
    fs::create_dir_all(home.join(".engram")).expect("engram dir");
    fs::create_dir_all(&sessions).expect("sessions");
    let log = store.join("watch.log");
    let yaml = |path: &Path| serde_json::to_string(&path.to_string_lossy()).expect("path");
    fs::write(
        home.join(".engram/config.yml"),
        format!(
            "db: {}\ntapes_dir: {}\nwatch:\n  debounce_secs: 1\n  ingest_timeout_secs: 60\n  log: {}\n  sources:\n    - path: {}\n      pattern: \"*.jsonl\"\n",
            yaml(&store.join("index.sqlite")),
            yaml(&store.join("tapes")),
            yaml(&log),
            yaml(&sessions),
        ),
    )
    .expect("config");

    // A stuck file: it has a cursor and bytes past it that no write will announce.
    let stuck = sessions.join("stuck.codex.jsonl");
    fs::write(&stuck, &fixture.undamaged_prefix).expect("prefix");
    engram_json(&home, &home, &["ingest", sessions.to_str().expect("utf8")]);
    append(&stuck, &fixture.bytes[fixture.undamaged_prefix.len()..]);
    assert!(!found(&home, &home, P3));

    let stderr = fs::File::create(store.join("watch.stderr")).expect("stderr file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    command
        .current_dir(&home)
        .arg("watch")
        .stdout(std::process::Stdio::null())
        .stderr(stderr);
    if cfg!(windows) {
        command.env("USERPROFILE", &home).env_remove("HOME");
    } else {
        command.env("HOME", &home);
    }
    let _watch = Watch(command.spawn().expect("watch spawns"));

    wait_until("startup catch-up", &log, || {
        partial_logged(&log, "stuck.codex.jsonl")
    });
    assert!(found(&home, &home, P3));

    // The next write to it is indexed as usual.
    append(&stuck, &fixture.pending_rest);
    wait_until("append indexed", &log, || found(&home, &home, P4));

    // A damaged file that arrives while watching is logged as partial too.
    fs::write(sessions.join("arrived.codex.jsonl"), &fixture.bytes).expect("arrived");
    wait_until("new file ingested", &log, || {
        partial_logged(&log, "arrived.codex.jsonl")
    });
    assert!(
        !fs::read_to_string(&log)
            .expect("log")
            .lines()
            .any(|line| line.contains("ingest ok path=") && line.contains("codex.jsonl")),
        "a damaged file was logged as ok"
    );
}
