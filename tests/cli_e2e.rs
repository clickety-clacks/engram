use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use engram::anchor::{fingerprint_anchor_hashes, fingerprint_similarity, fingerprint_text};
use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn run_cli(repo: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engram"));
    let isolated_home = repo.join(".home");
    fs::create_dir_all(&isolated_home).expect("home dir");
    cmd.current_dir(repo).args(args);
    cmd.env("HOME", &isolated_home);
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
    let span_text = "fn omega() { return value + 1; }";
    fs::write(
        repo.join("src/lib.rs"),
        format!("alpha\n{span_text}\nzeta\n"),
    )
    .expect("seed file");

    let init = run_json(repo, &["init"], None);
    assert_eq!(init["status"], "ok");

    let span_anchor = fingerprint_text(span_text).fingerprint;
    let transcript = format!(
        concat!(
            "{{\"t\":\"2026-02-22T00:00:00Z\",\"k\":\"meta\",\"model\":\"gpt-5\",\"repo_head\":\"abc123\",\"label\":\"lane-c\"}}\n",
            "{{\"t\":\"2026-02-22T00:00:01Z\",\"k\":\"code.read\",\"file\":\"src/lib.rs\",\"range\":[2,2],\"anchor_hashes\":[\"{0}\"]}}\n",
            "{{\"t\":\"2026-02-22T00:00:02Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",\"before_range\":[2,2],\"after_range\":[2,2],\"before_anchor_hashes\":[\"winnow:00000000000000aa\"],\"after_anchor_hashes\":[\"{0}\"]}}\n",
            "{{\"t\":\"2026-02-22T00:00:03Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",\"before_range\":[4,5],\"after_range\":[4,5],\"before_anchor_hashes\":[\"winnow:00000000000000bb\"]}}\n"
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
    assert!(query_anchors.len() >= 1);
    assert!(query_anchors.iter().any(|anchor| anchor == &span_anchor));
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0]["touch_count"].as_u64().unwrap_or(0) >= 1);
}

#[test]
fn explain_matches_winnow_edit_anchor_for_span_targets() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir");
    let span_text = "fn omega() { return value + 1; }";
    fs::write(
        repo.join("src/lib.rs"),
        format!("alpha\n{span_text}\nzeta\n"),
    )
    .expect("seed file");

    let _ = run_json(repo, &["init"], None);

    let span_anchor = fingerprint_text(span_text).fingerprint;
    let transcript = format!(
        concat!(
            "{{\"t\":\"2026-02-22T00:00:00Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",",
            "\"before_range\":[2,2],\"after_range\":[2,2],",
            "\"before_anchor_hashes\":[\"winnow:00000000000000cc\"],",
            "\"after_anchor_hashes\":[\"{0}\"],\"similarity\":0.95}}\n"
        ),
        span_anchor
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(&transcript));

    let explain = run_json(repo, &["explain", "src/lib.rs:2-2"], None);
    let query_anchors = explain["query"]["anchors"].as_array().expect("anchors");
    assert!(
        query_anchors
            .iter()
            .any(|anchor| anchor == &Value::String(span_anchor.clone())),
        "expected winnow anchor in query anchors"
    );
    assert_eq!(
        explain["sessions"].as_array().expect("sessions").len(),
        1,
        "expected explain to recover session via winnow edit anchor"
    );
    assert_eq!(
        explain["lineage"].as_array().expect("lineage").len(),
        1,
        "expected inbound edit linkage for matched winnow anchor"
    );
}

#[test]
fn explain_matches_winnow_edit_anchor_for_multiline_span_with_trailing_newline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "fn a() {\n    alpha();\n}\n").expect("seed file");

    let _ = run_json(repo, &["init"], None);

    let exact_span = "fn a() {\n    alpha();\n}\n";
    let exact_anchor = fingerprint_text(exact_span).fingerprint;
    let transcript = format!(
        concat!(
            "{{\"t\":\"2026-02-22T00:00:00Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",",
            "\"before_range\":[1,3],\"after_range\":[1,3],",
            "\"before_anchor_hashes\":[\"winnow:00000000000000dd\"],",
            "\"after_anchor_hashes\":[\"{0}\"],\"similarity\":0.95}}\n"
        ),
        exact_anchor
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(&transcript));

    let explain = run_json(repo, &["explain", "src/lib.rs:1-3"], None);
    let query_anchors = explain["query"]["anchors"].as_array().expect("anchors");
    assert!(
        query_anchors
            .iter()
            .any(|anchor| anchor == &Value::String(exact_anchor.clone())),
        "expected exact multiline winnow anchor in query anchors"
    );
    assert_eq!(
        explain["sessions"].as_array().expect("sessions").len(),
        1,
        "expected explain to recover session via exact multiline winnow anchor"
    );
    assert_eq!(
        explain["lineage"].as_array().expect("lineage").len(),
        1,
        "expected inbound edit linkage for exact multiline winnow anchor"
    );
}

#[test]
fn explain_matches_windowed_edit_anchor_for_arbitrary_subspan() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir");

    let file_text = (1..=72)
        .map(|line| format!("fn line_{line}() {{ value_{line}(); }}\n"))
        .collect::<String>();
    fs::write(repo.join("src/lib.rs"), &file_text).expect("seed file");

    let _ = run_json(repo, &["init"], None);

    let after_anchor_hashes = fingerprint_anchor_hashes(&file_text);
    assert!(
        after_anchor_hashes.len() >= 3,
        "anchors={after_anchor_hashes:?}"
    );

    let transcript = format!(
        concat!(
            "{{\"t\":\"2026-02-22T00:00:00Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",",
            "\"before_range\":[1,24],\"after_range\":[1,24],",
            "\"before_text\":\"fn old() {{ legacy(); }}\\n\",",
            "\"after_text\":{0},\"similarity\":0.95}}\n"
        ),
        serde_json::to_string(&file_text).expect("text")
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(&transcript));

    let explain = run_json(repo, &["explain", "src/lib.rs:25-32"], None);
    let query_anchors = explain["query"]["anchors"]
        .as_array()
        .expect("anchors")
        .iter()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert!(
        after_anchor_hashes
            .iter()
            .any(|stored_anchor| query_anchors
                .iter()
                .any(
                    |query_anchor| fingerprint_similarity(query_anchor, stored_anchor)
                        .is_some_and(|score| score > 0.0)
                )),
        "expected query anchors to overlap stored windowed anchors by similarity: query={query_anchors:?} stored={after_anchor_hashes:?}"
    );
    assert_eq!(
        explain["sessions"].as_array().expect("sessions").len(),
        1,
        "expected explain to recover session via overlapping windowed anchor"
    );
}

#[test]
fn large_windowed_file_stays_under_row_budget_and_explains() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir");

    let file_text = (1..=1914)
        .map(|line| format!("fn line_{line}() {{ value_{line}(); }}\n"))
        .collect::<String>();
    fs::write(repo.join("src/lib.rs"), &file_text).expect("seed file");

    let _ = run_json(repo, &["init"], None);
    let transcript = format!(
        concat!(
            "{{\"t\":\"2026-02-22T00:00:00Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",",
            "\"before_range\":[1,1],\"after_range\":[1,1914],",
            "\"before_text\":\"fn old() {{ legacy(); }}\\n\",",
            "\"after_text\":{0},\"similarity\":0.95}}\n"
        ),
        serde_json::to_string(&file_text).expect("text")
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(&transcript));

    let conn = Connection::open(repo.join(".home/.engram/index.sqlite")).expect("sqlite");
    let evidence_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM evidence WHERE file_path = 'src/lib.rs'",
            [],
            |row| row.get(0),
        )
        .expect("evidence count");
    assert!(evidence_rows > 0);
    assert!(
        evidence_rows < 500,
        "expected window-scale evidence rows, got {evidence_rows}"
    );

    let explain = run_json(repo, &["explain", "src/lib.rs:900-930"], None);
    assert!(
        !explain["sessions"].as_array().expect("sessions").is_empty(),
        "expected explain to recover at least one session"
    );
}

#[test]
fn explain_include_deleted_controls_tombstones() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let _ = run_json(repo, &["init"], None);

    let deleted_anchor = "winnow:00000000000000de";
    let transcript = format!(
        concat!(
            "{{\"t\":\"2026-02-22T00:00:00Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",",
            "\"before_range\":[10,12],\"after_range\":[10,12],",
            "\"before_anchor_hashes\":[\"{0}\"]}}\n"
        ),
        deleted_anchor
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(&transcript));

    let without_deleted = run_json(repo, &["explain", deleted_anchor, "--anchor"], None);
    assert_eq!(
        without_deleted["tombstones"]
            .as_array()
            .expect("tombstones array")
            .len(),
        0
    );

    let with_deleted = run_json(
        repo,
        &["explain", deleted_anchor, "--anchor", "--include-deleted"],
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

    let from_anchor = "winnow:00000000000000ef";
    let to_anchor = "winnow:00000000000000f0";
    let transcript = format!(
        concat!(
            "{{\"t\":\"2026-02-22T00:00:00Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",",
            "\"before_range\":[1,1],\"after_range\":[1,1],",
            "\"before_anchor_hashes\":[\"{0}\"],\"after_anchor_hashes\":[\"{1}\"]}}\n",
            "{{\"t\":\"2026-02-22T00:00:01Z\",\"k\":\"span.link\",\"from_file\":\"src/a.rs\",",
            "\"from_range\":[1,2],\"to_file\":\"src/b.rs\",\"to_range\":[10,20],\"note\":\"extract\"}}\n"
        ),
        from_anchor, to_anchor
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(&transcript));

    let default_explain = run_json(repo, &["explain", to_anchor, "--anchor"], None);
    assert_eq!(
        default_explain["lineage"]
            .as_array()
            .expect("lineage array")
            .len(),
        0
    );

    let forensics_explain = run_json(
        repo,
        &["explain", to_anchor, "--anchor", "--forensics"],
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
fn record_command_captures_tool_events_and_exit_status() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let _ = run_json(repo, &["init"], None);

    let record = run_json(
        repo,
        &[
            "record",
            "/bin/sh",
            "-c",
            "printf 'stdout-msg'; printf 'stderr-msg' 1>&2; exit 7",
        ],
        None,
    );
    let tape_id = record["tape_id"].as_str().expect("tape id");
    assert_eq!(record["status"], "ok");
    assert_eq!(record["recorded_command"]["exit"], 7);
    assert_eq!(record["recorded_command"]["success"], false);
    assert_eq!(record["recorded_command"]["argv"][0], "/bin/sh");

    let raw = run_cli(repo, &["show", tape_id, "--raw"], None);
    assert!(raw.status.success(), "show --raw should succeed");
    let raw_text = String::from_utf8_lossy(&raw.stdout);
    assert!(raw_text.contains("\"k\":\"tool.call\""));
    assert!(raw_text.contains("\"k\":\"tool.result\""));
    assert!(raw_text.contains("\"exit\":7"));
    assert!(raw_text.contains("stdout-msg"));
    assert!(raw_text.contains("stderr-msg"));
}

#[test]
fn explain_orders_sessions_by_touch_count_then_recency() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let _ = run_json(repo, &["init"], None);

    let anchor = "ordering-anchor";
    let tape_one = format!(
        concat!(
            "{{\"t\":\"2026-02-22T00:00:00Z\",\"k\":\"meta\",\"label\":\"first\"}}\n",
            "{{\"t\":\"2026-02-22T00:00:01Z\",\"k\":\"code.read\",\"file\":\"src/lib.rs\",\"range\":[1,1],\"anchor_hashes\":[\"{0}\"]}}\n"
        ),
        anchor
    );
    let tape_two = format!(
        concat!(
            "{{\"t\":\"2026-02-22T00:00:10Z\",\"k\":\"meta\",\"label\":\"second\"}}\n",
            "{{\"t\":\"2026-02-22T00:00:11Z\",\"k\":\"code.read\",\"file\":\"src/lib.rs\",\"range\":[1,1],\"anchor_hashes\":[\"{0}\"]}}\n",
            "{{\"t\":\"2026-02-22T00:00:12Z\",\"k\":\"code.read\",\"file\":\"src/lib.rs\",\"range\":[2,2],\"anchor_hashes\":[\"{0}\"]}}\n"
        ),
        anchor
    );
    let _ = run_json(repo, &["record", "--stdin"], Some(&tape_one));
    let second = run_json(repo, &["record", "--stdin"], Some(&tape_two));

    let explain = run_json(repo, &["explain", anchor, "--anchor"], None);
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0]["touch_count"], 2);
    assert_eq!(
        sessions[0]["tape_id"], second["tape_id"],
        "higher touch-count tape should rank first"
    );
    assert_eq!(sessions[1]["touch_count"], 1);
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
    fs::create_dir_all(tape_path.parent().expect("tape parent")).expect("tape dir");
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

#[test]
fn record_command_captures_tool_events_and_persists_tape() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let _ = run_json(repo, &["init"], None);

    let record = run_json(repo, &["record", "/bin/sh", "-c", "printf hello"], None);
    assert_eq!(record["status"], "ok");
    assert_eq!(record["record"]["mode"], "command");
    assert_eq!(record["record"]["success"], true);
    assert_eq!(record["record"]["exit_code"], 0);

    let tape_id = record["tape_id"].as_str().expect("tape id");
    let show_raw = run_cli(repo, &["show", tape_id, "--raw"], None);
    assert!(show_raw.status.success(), "show --raw should succeed");
    let raw = String::from_utf8_lossy(&show_raw.stdout);
    assert!(raw.contains("\"k\":\"tool.call\""), "raw={raw}");
    assert!(raw.contains("\"k\":\"tool.result\""), "raw={raw}");
    assert!(raw.contains("\"stdout\":\"hello\""), "raw={raw}");
}

#[test]
fn record_command_keeps_trace_for_failed_process() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let _ = run_json(repo, &["init"], None);

    let record = run_json(
        repo,
        &["record", "/bin/sh", "-c", "echo boom >&2; exit 7"],
        None,
    );
    assert_eq!(record["status"], "ok");
    assert_eq!(record["record"]["mode"], "command");
    assert_eq!(record["record"]["success"], false);
    assert_eq!(record["record"]["exit_code"], 7);

    let tape_id = record["tape_id"].as_str().expect("tape id");
    let show_raw = run_cli(repo, &["show", tape_id, "--raw"], None);
    assert!(show_raw.status.success(), "show --raw should succeed");
    let raw = String::from_utf8_lossy(&show_raw.stdout);
    assert!(raw.contains("\"k\":\"tool.call\""), "raw={raw}");
    assert!(raw.contains("\"k\":\"tool.result\""), "raw={raw}");
    assert!(raw.contains("\"exit\":7"), "raw={raw}");
    assert!(raw.contains("\"stderr\":\"boom\\n\""), "raw={raw}");
}

#[test]
fn global_and_dispatch_flags_are_removed_from_cli() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    let ingest_global = run_cli(repo, &["ingest", "--global"], None);
    assert!(!ingest_global.status.success());
    let ingest_err = String::from_utf8_lossy(&ingest_global.stderr);
    assert!(ingest_err.contains("--global"), "stderr={ingest_err}");

    let explain_dispatch = run_cli(repo, &["explain", "--dispatch", "abc"], None);
    assert!(!explain_dispatch.status.success());
    let explain_err = String::from_utf8_lossy(&explain_dispatch.stderr);
    assert!(explain_err.contains("--dispatch"), "stderr={explain_err}");
}

#[test]
fn watch_errors_clearly_when_watch_config_is_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    let watch = run_cli(repo, &["watch"], None);
    assert!(
        !watch.status.success(),
        "watch should fail without watch config"
    );
    let stderr = String::from_utf8_lossy(&watch.stderr);
    assert!(
        stderr.contains("watch config missing in config.yml"),
        "stderr={stderr}"
    );
}
