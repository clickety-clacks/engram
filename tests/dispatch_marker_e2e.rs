use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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

fn sha256_hex(input: &str) -> String {
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

fn collect_session_ids_from_windows(explain: &Value) -> HashSet<String> {
    let mut out = HashSet::new();
    let sessions = explain["sessions"].as_array().cloned().unwrap_or_default();
    for session in sessions {
        let windows = session["windows"].as_array().cloned().unwrap_or_default();
        for window in windows {
            let events = window["events"].as_array().cloned().unwrap_or_default();
            for event in events {
                if let Some(session_id) = event
                    .get("event")
                    .and_then(|v| v.get("source"))
                    .and_then(|v| v.get("session_id"))
                    .and_then(Value::as_str)
                {
                    out.insert(session_id.to_string());
                }
            }
        }
    }
    out
}

#[test]
fn explain_dispatch_chain_includes_a_to_b_to_c_and_excludes_sibling() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::create_dir_all(repo.join("inputs")).expect("inputs dir");
    fs::write(
        repo.join("src/engine.rs"),
        "fn helper() {}\npub fn continuation_probe() -> &'static str { \"T126\" }\n",
    )
    .expect("seed file");

    let _ = run_json(repo, &["init"], None);

    let uuid_ab = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let uuid_bc = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let span_text = "pub fn continuation_probe() -> &'static str { \"T126\" }";
    let span_sha = sha256_hex(span_text);

    fs::write(
        repo.join("inputs/a.jsonl"),
        format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"oc-a\",\"timestamp\":\"2026-02-27T12:00:00Z\"}}\n",
                "{{\"type\":\"message\",\"id\":\"a1\",\"timestamp\":\"2026-02-27T12:00:01Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"id\":\"call_a\",\"name\":\"exec\",\"arguments\":{{\"cmd\":\"tmux send-keys \\\"<engram-src id=\\\\\\\"{0}\\\\\\\"/> do work\\\"\"}}}}]}}}}\n"
            ),
            uuid_ab
        ),
    )
    .expect("write a");

    fs::write(
        repo.join("inputs/b.jsonl"),
        format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"oc-b\",\"timestamp\":\"2026-02-27T12:05:00Z\"}}\n",
                "{{\"type\":\"message\",\"id\":\"b1\",\"timestamp\":\"2026-02-27T12:05:01Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"<engram-src id=\\\"{0}\\\"/> please implement\"}}]}}}}\n",
                "{{\"type\":\"message\",\"id\":\"b2\",\"timestamp\":\"2026-02-27T12:05:02Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"id\":\"call_b\",\"name\":\"exec\",\"arguments\":{{\"cmd\":\"tmux send-keys \\\"<engram-src id=\\\\\\\"{1}\\\\\\\"/> continue\\\"\"}}}}]}}}}\n"
            ),
            uuid_ab, uuid_bc
        ),
    )
    .expect("write b");

    fs::write(
        repo.join("inputs/c.jsonl"),
        format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"oc-c\",\"timestamp\":\"2026-02-27T12:10:00Z\"}}\n",
                "{{\"type\":\"message\",\"id\":\"c1\",\"timestamp\":\"2026-02-27T12:10:01Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"<engram-src id=\\\"{0}\\\"/> execute\"}}]}}}}\n",
                "{{\"type\":\"message\",\"id\":\"c2\",\"timestamp\":\"2026-02-27T12:10:02Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"id\":\"call_c\",\"name\":\"Apply\",\"arguments\":{{\"file\":\"src/engine.rs\",\"before_hash\":\"old-c\",\"after_hash\":\"{1}\"}}}}]}}}}\n",
                "{{\"type\":\"message\",\"id\":\"c3\",\"timestamp\":\"2026-02-27T12:10:03Z\",\"message\":{{\"role\":\"toolResult\",\"toolCallId\":\"call_c\",\"toolName\":\"Apply\",\"content\":[{{\"type\":\"text\",\"text\":\"updated src/engine.rs\"}}],\"isError\":false}}}}\n"
            ),
            uuid_bc, span_sha
        ),
    )
    .expect("write c");

    fs::write(
        repo.join("inputs/sibling.jsonl"),
        format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"oc-sibling\",\"timestamp\":\"2026-02-27T12:06:00Z\"}}\n",
                "{{\"type\":\"message\",\"id\":\"s1\",\"timestamp\":\"2026-02-27T12:06:01Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"<engram-src id=\\\"{0}\\\"/> sibling\"}}]}}}}\n"
            ),
            uuid_ab
        ),
    )
    .expect("write sibling");

    fs::write(
        repo.join(".engram/config.yml"),
        format!(
            "sources:\n  - path: {}/inputs/*.jsonl\n    adapter: openclaw\nexclude: []\n",
            repo.display()
        ),
    )
    .expect("write config");

    let ingest = run_json(repo, &["ingest"], None);
    assert_eq!(ingest["status"], "ok");
    assert_eq!(ingest["imported_tapes"], 4);

    let explain = run_json(repo, &["explain", "src/engine.rs:2-2"], None);
    let ids = collect_session_ids_from_windows(&explain);
    assert!(ids.contains("oc-c"), "ids={ids:?}");
    assert!(ids.contains("oc-b"), "ids={ids:?}");
    assert!(ids.contains("oc-a"), "ids={ids:?}");
    assert!(!ids.contains("oc-sibling"), "ids={ids:?}");

    let dispatch_lineage = explain["dispatch_lineage"]
        .as_array()
        .expect("dispatch lineage array");
    assert!(dispatch_lineage.len() >= 2, "dispatch_lineage={dispatch_lineage:?}");

    let dispatch_query = run_json(repo, &["explain", "--dispatch", uuid_bc], None);
    let dispatch_sessions = dispatch_query["sessions"].as_array().expect("sessions");
    assert_eq!(dispatch_sessions.len(), 2);
    let spans = dispatch_query["spans"].as_array().expect("spans");
    assert!(
        spans
            .iter()
            .any(|span| span["file"] == "src/engine.rs" && span["kind"] == "edit"),
        "spans={spans:?}"
    );
}

#[test]
fn compact_restart_reingest_adds_new_tape_without_duplication() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::create_dir_all(repo.join("inputs")).expect("inputs dir");
    fs::write(
        repo.join("src/engine.rs"),
        "fn helper() {}\npub fn continuation_probe() -> &'static str { \"T126R\" }\n",
    )
    .expect("seed file");

    let _ = run_json(repo, &["init"], None);
    let uuid = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let span_text = "pub fn continuation_probe() -> &'static str { \"T126R\" }";
    let span_sha = sha256_hex(span_text);

    fs::write(
        repo.join("inputs/base.jsonl"),
        format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"oc-base\",\"timestamp\":\"2026-02-27T13:00:00Z\"}}\n",
                "{{\"type\":\"message\",\"id\":\"b1\",\"timestamp\":\"2026-02-27T13:00:01Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"id\":\"call_base\",\"name\":\"exec\",\"arguments\":{{\"cmd\":\"tmux send-keys \\\"<engram-src id=\\\\\\\"{0}\\\\\\\"/>\\\"\"}}}}]}}}}\n",
                "{{\"type\":\"message\",\"id\":\"b2\",\"timestamp\":\"2026-02-27T13:00:02Z\",\"message\":{{\"role\":\"toolResult\",\"toolCallId\":\"call_base\",\"toolName\":\"exec\",\"content\":[{{\"type\":\"text\",\"text\":\"ok\"}}],\"isError\":false}}}}\n"
            ),
            uuid
        ),
    )
    .expect("write base");

    fs::write(
        repo.join("inputs/worker.jsonl"),
        format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"oc-worker\",\"timestamp\":\"2026-02-27T13:05:00Z\"}}\n",
                "{{\"type\":\"message\",\"id\":\"w1\",\"timestamp\":\"2026-02-27T13:05:01Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"<engram-src id=\\\"{0}\\\"/> run\"}}]}}}}\n",
                "{{\"type\":\"message\",\"id\":\"w2\",\"timestamp\":\"2026-02-27T13:05:02Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"id\":\"call_w\",\"name\":\"Apply\",\"arguments\":{{\"file\":\"src/engine.rs\",\"before_hash\":\"old-w\",\"after_hash\":\"{1}\"}}}}]}}}}\n",
                "{{\"type\":\"message\",\"id\":\"w3\",\"timestamp\":\"2026-02-27T13:05:03Z\",\"message\":{{\"role\":\"toolResult\",\"toolCallId\":\"call_w\",\"toolName\":\"Apply\",\"content\":[{{\"type\":\"text\",\"text\":\"updated\"}}],\"isError\":false}}}}\n"
            ),
            uuid, span_sha
        ),
    )
    .expect("write worker");

    fs::write(
        repo.join(".engram/config.yml"),
        format!(
            "sources:\n  - path: {}/inputs/*.jsonl\n    adapter: openclaw\nexclude: []\n",
            repo.display()
        ),
    )
    .expect("write config");

    let first = run_json(repo, &["ingest"], None);
    assert_eq!(first["imported_tapes"], 2);

    let second = run_json(repo, &["ingest"], None);
    assert_eq!(second["imported_tapes"], 0);
    assert_eq!(second["skipped_unchanged"], 2);

    fs::write(
        repo.join("inputs/worker-restart.jsonl"),
        format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"oc-worker-restart\",\"timestamp\":\"2026-02-27T13:20:00Z\"}}\n",
                "{{\"type\":\"message\",\"id\":\"r1\",\"timestamp\":\"2026-02-27T13:20:01Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"<engram-src id=\\\"{0}\\\"/> resumed after compact\"}}]}}}}\n",
                "{{\"type\":\"message\",\"id\":\"r2\",\"timestamp\":\"2026-02-27T13:20:02Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"id\":\"call_r\",\"name\":\"Apply\",\"arguments\":{{\"file\":\"src/engine.rs\",\"before_hash\":\"old-r\",\"after_hash\":\"{1}\"}}}}]}}}}\n",
                "{{\"type\":\"message\",\"id\":\"r3\",\"timestamp\":\"2026-02-27T13:20:03Z\",\"message\":{{\"role\":\"toolResult\",\"toolCallId\":\"call_r\",\"toolName\":\"Apply\",\"content\":[{{\"type\":\"text\",\"text\":\"updated restart\"}}],\"isError\":false}}}}\n"
            ),
            uuid, span_sha
        ),
    )
    .expect("write restart");

    let third = run_json(repo, &["ingest"], None);
    assert_eq!(third["imported_tapes"], 1);

    let dispatch_query = run_json(repo, &["explain", "--dispatch", uuid], None);
    assert_eq!(
        dispatch_query["sessions"]
            .as_array()
            .expect("sessions")
            .len(),
        3
    );
}
