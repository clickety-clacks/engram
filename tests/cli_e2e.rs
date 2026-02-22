use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use engram::anchor::fingerprint_text;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn run_cli(repo: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engram"));
    cmd.current_dir(repo).args(args);
    if stdin.is_none() {
        return cmd.output().expect("command runs");
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("command spawns");
    {
        let mut pipe = child.stdin.take().expect("stdin pipe");
        pipe.write_all(stdin.expect("stdin content").as_bytes())
            .expect("stdin write");
    }
    child.wait_with_output().expect("command output")
}

fn run_json(repo: &Path, args: &[&str], stdin: Option<&str>) -> Value {
    let output = run_cli(repo, args, stdin);
    assert!(
        output.status.success(),
        "command failed: args={args:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json stdout")
}

fn tape_id_for_contents(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[test]
fn init_record_tapes_show_and_explain_roundtrip() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "alpha\nomega\nzeta\n").expect("seed file");

    let init = run_json(repo, &["init"], None);
    assert_eq!(init["status"], "ok");

    let span_anchor = fingerprint_text("omega").fingerprint;
    let transcript = format!(
        concat!(
            "{{\"t\":\"2026-02-22T00:00:00Z\",\"k\":\"meta\",\"model\":\"gpt-5\",\"repo_head\":\"abc123\",\"label\":\"lane-c\"}}\n",
            "{{\"t\":\"2026-02-22T00:00:01Z\",\"k\":\"code.read\",\"file\":\"src/lib.rs\",\"range\":[2,2],\"anchor_hashes\":[\"{0}\"]}}\n",
            "{{\"t\":\"2026-02-22T00:00:02Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",\"before_range\":[2,2],\"after_range\":[2,2],\"before_hash\":\"seed\",\"after_hash\":\"{0}\"}}\n",
            "{{\"t\":\"2026-02-22T00:00:03Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",\"before_range\":[4,5],\"after_range\":[4,5],\"before_hash\":\"gone\"}}\n"
        ),
        span_anchor
    );

    let record = run_json(repo, &["record", "--stdin"], Some(&transcript));
    let tape_id = record["tape_id"].as_str().expect("tape id");
    assert_eq!(record["status"], "ok");
    assert_eq!(record["event_count"], 4);
    assert_eq!(record["meta"]["model"], "gpt-5");

    let tapes = run_json(repo, &["tapes"], None);
    let tape_list = tapes["tapes"].as_array().expect("tapes array");
    assert_eq!(tape_list.len(), 1);
    assert_eq!(tape_list[0]["tape_id"], tape_id);
    assert_eq!(tape_list[0]["meta"]["label"], "lane-c");

    let show = run_json(repo, &["show", tape_id], None);
    assert_eq!(show["event_count"], 4);
    assert_eq!(show["events"].as_array().expect("events").len(), 4);
    assert_eq!(show["meta"]["repo_head"], "abc123");

    let raw = run_cli(repo, &["show", tape_id, "--raw"], None);
    assert!(raw.status.success(), "show --raw should succeed");
    assert_eq!(String::from_utf8_lossy(&raw.stdout), transcript);

    let explain = run_json(repo, &["explain", "src/lib.rs:2-2"], None);
    let query_anchors = explain["query"]["anchors"].as_array().expect("anchors");
    assert_eq!(query_anchors.len(), 1);
    assert_eq!(query_anchors[0], span_anchor);
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0]["touch_count"].as_u64().unwrap_or(0) >= 1);
}

#[test]
fn explain_include_deleted_controls_tombstones() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let _ = run_json(repo, &["init"], None);

    let transcript = concat!(
        r#"{"t":"2026-02-22T00:00:00Z","k":"code.edit","file":"src/lib.rs","before_range":[10,12],"after_range":[10,12],"before_hash":"deleted-anchor"}"#,
        "\n"
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(transcript));

    let without_deleted = run_json(repo, &["explain", "deleted-anchor", "--anchor"], None);
    assert_eq!(
        without_deleted["tombstones"]
            .as_array()
            .expect("tombstones array")
            .len(),
        0
    );

    let with_deleted = run_json(
        repo,
        &["explain", "deleted-anchor", "--anchor", "--include-deleted"],
        None,
    );
    let tombstones = with_deleted["tombstones"].as_array().expect("tombstones");
    assert_eq!(tombstones.len(), 1);
    assert_eq!(tombstones[0]["file_path"], "src/lib.rs");
    assert_eq!(tombstones[0]["range"]["start"], 10);
}

#[test]
fn explain_forensics_and_agent_links_behave_as_specified() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let _ = run_json(repo, &["init"], None);

    let transcript = concat!(
        r#"{"t":"2026-02-22T00:00:00Z","k":"code.edit","file":"src/lib.rs","before_range":[1,1],"after_range":[1,1],"before_hash":"from-anchor","after_hash":"to-anchor"}"#,
        "\n",
        r#"{"t":"2026-02-22T00:00:01Z","k":"span.link","from_file":"src/a.rs","from_range":[1,2],"to_file":"src/b.rs","to_range":[10,20],"note":"extract"}"#,
        "\n"
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(transcript));

    let default_explain = run_json(repo, &["explain", "to-anchor", "--anchor"], None);
    assert_eq!(
        default_explain["lineage"]
            .as_array()
            .expect("lineage array")
            .len(),
        0
    );

    let forensics_explain = run_json(
        repo,
        &["explain", "to-anchor", "--anchor", "--forensics"],
        None,
    );
    assert_eq!(
        forensics_explain["lineage"]
            .as_array()
            .expect("lineage array")
            .len(),
        1
    );

    let agent_link = run_json(
        repo,
        &[
            "explain",
            "span:src/b.rs:10-20",
            "--anchor",
            "--min-confidence",
            "0.99",
        ],
        None,
    );
    let lineage = agent_link["lineage"].as_array().expect("lineage");
    assert_eq!(lineage.len(), 1);
    assert_eq!(lineage[0]["agent_link"], true);
}

#[test]
fn gc_removes_unreferenced_tapes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let _ = run_json(repo, &["init"], None);

    let referenced = concat!(
        r#"{"t":"2026-02-22T00:00:00Z","k":"code.read","file":"src/lib.rs","range":[1,1],"anchor_hashes":["anchor-1"]}"#,
        "\n"
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(referenced));

    let unreferenced = concat!(
        r#"{"t":"2026-02-22T00:00:00Z","k":"meta","model":"gpt-5"}"#,
        "\n"
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(unreferenced));

    let before = run_json(repo, &["tapes"], None);
    assert_eq!(before["tapes"].as_array().expect("tapes").len(), 2);

    let gc = run_json(repo, &["gc"], None);
    assert_eq!(gc["deleted_count"], 1);

    let after = run_json(repo, &["tapes"], None);
    assert_eq!(after["tapes"].as_array().expect("tapes").len(), 1);
}

#[test]
fn record_recovers_when_tape_file_exists_but_index_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let _ = run_json(repo, &["init"], None);

    let transcript = concat!(
        r#"{"t":"2026-02-22T00:00:00Z","k":"code.read","file":"src/lib.rs","range":[1,1],"anchor_hashes":["orphan-anchor"]}"#,
        "\n"
    );
    let tape_id = tape_id_for_contents(transcript);
    let tape_path = repo
        .join(".engram")
        .join("tapes")
        .join(format!("{tape_id}.jsonl.zst"));
    let compressed = zstd::stream::encode_all(transcript.as_bytes(), 0).expect("compress");
    fs::write(&tape_path, compressed).expect("write tape");

    let explain_before = run_json(repo, &["explain", "orphan-anchor", "--anchor"], None);
    assert_eq!(
        explain_before["sessions"]
            .as_array()
            .expect("sessions")
            .len(),
        0
    );

    let record = run_json(repo, &["record", "--stdin"], Some(transcript));
    assert_eq!(record["already_exists"], false);
    assert_eq!(record["already_indexed"], false);
    assert_eq!(record["tape_file_exists"], true);

    let explain_after = run_json(repo, &["explain", "orphan-anchor", "--anchor"], None);
    assert_eq!(
        explain_after["sessions"]
            .as_array()
            .expect("sessions")
            .len(),
        1
    );
}
