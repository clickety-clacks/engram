use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use engram::anchor::fingerprint_text;
use serde_json::Value;

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

#[test]
fn explain_dispatch_chain_includes_a_to_b_to_c_and_excludes_sibling() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(
        repo.join("src/engine.rs"),
        "fn helper() {}\npub fn continuation_probe() -> &'static str { \"T126\" }\n",
    )
    .expect("seed file");

    let uuid_ab = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let uuid_bc = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let span_text = "pub fn continuation_probe() -> &'static str { \"T126\" }";
    let span_anchor = fingerprint_text(span_text).fingerprint;

    let tape_a = format!(
        concat!(
            "{{\"t\":\"2026-02-27T12:00:00Z\",\"k\":\"msg.out\",\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"arguments\":{{\"payload\":\"<engram-src id=\\\"{0}\\\"/> do work\"}}}}]}}\n"
        ),
        uuid_ab
    );
    let tape_b = format!(
        concat!(
            "{{\"t\":\"2026-02-27T12:05:01Z\",\"k\":\"msg.in\",\"role\":\"user\",\"content\":\"<engram-src id=\\\"{0}\\\"/> please implement\"}}\n",
            "{{\"t\":\"2026-02-27T12:05:02Z\",\"k\":\"msg.out\",\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"arguments\":{{\"payload\":\"<engram-src id=\\\"{1}\\\"/> continue\"}}}}]}}\n"
        ),
        uuid_ab, uuid_bc
    );
    let tape_c = format!(
        concat!(
            "{{\"t\":\"2026-02-27T12:10:01Z\",\"k\":\"msg.in\",\"role\":\"user\",\"content\":\"<engram-src id=\\\"{0}\\\"/> execute\"}}\n",
            "{{\"t\":\"2026-02-27T12:10:02Z\",\"k\":\"code.edit\",\"file\":\"src/engine.rs\",\"before_range\":[2,2],\"after_range\":[2,2],\"before_anchor_hashes\":[\"winnow:0000000000000101\"],\"after_anchor_hashes\":[\"{1}\"],\"similarity\":0.95}}\n"
        ),
        uuid_bc, span_anchor
    );
    let tape_sibling = format!(
        "{{\"t\":\"2026-02-27T12:06:01Z\",\"k\":\"msg.in\",\"role\":\"user\",\"content\":\"<engram-src id=\\\"{uuid_ab}\\\"/> sibling\"}}\n"
    );

    let _ = run_json(repo, &["record", "--stdin"], Some(&tape_a));
    let _ = run_json(repo, &["record", "--stdin"], Some(&tape_b));
    let _ = run_json(repo, &["record", "--stdin"], Some(&tape_c));
    let _ = run_json(repo, &["record", "--stdin"], Some(&tape_sibling));

    let explain = run_json(repo, &["explain", "src/engine.rs:2-2"], None);
    assert_eq!(explain["sessions"].as_array().expect("sessions").len(), 3);

    let dispatch_lineage = explain["dispatch_lineage"]
        .as_array()
        .expect("dispatch lineage array");
    assert!(
        dispatch_lineage.len() >= 2,
        "dispatch_lineage={dispatch_lineage:?}"
    );
    assert!(
        dispatch_lineage
            .iter()
            .any(|hop| hop["received_uuid"] == uuid_bc),
        "dispatch_lineage={dispatch_lineage:?}"
    );
    assert!(
        dispatch_lineage
            .iter()
            .any(|hop| hop["received_uuid"] == uuid_ab),
        "dispatch_lineage={dispatch_lineage:?}"
    );
}

#[test]
fn compact_restart_reingest_adds_new_tape_without_duplication() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(
        repo.join("src/engine.rs"),
        "fn helper() {}\npub fn continuation_probe() -> &'static str { \"T126R\" }\n",
    )
    .expect("seed file");

    let uuid = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let span_text = "pub fn continuation_probe() -> &'static str { \"T126R\" }";
    let span_anchor = fingerprint_text(span_text).fingerprint;

    let tape_base = format!(
        "{{\"t\":\"2026-02-27T13:00:01Z\",\"k\":\"msg.out\",\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"arguments\":{{\"payload\":\"<engram-src id=\\\"{uuid}\\\"/>\"}}}}]}}\n"
    );
    let tape_worker = format!(
        concat!(
            "{{\"t\":\"2026-02-27T13:05:01Z\",\"k\":\"msg.in\",\"role\":\"user\",\"content\":\"<engram-src id=\\\"{0}\\\"/> run\"}}\n",
            "{{\"t\":\"2026-02-27T13:05:02Z\",\"k\":\"code.edit\",\"file\":\"src/engine.rs\",\"before_range\":[2,2],\"after_range\":[2,2],\"before_anchor_hashes\":[\"winnow:0000000000000102\"],\"after_anchor_hashes\":[\"{1}\"],\"similarity\":0.95}}\n"
        ),
        uuid, span_anchor
    );
    let tape_restart = format!(
        concat!(
            "{{\"t\":\"2026-02-27T13:20:01Z\",\"k\":\"msg.in\",\"role\":\"user\",\"content\":\"<engram-src id=\\\"{0}\\\"/> resumed after compact\"}}\n",
            "{{\"t\":\"2026-02-27T13:20:02Z\",\"k\":\"code.edit\",\"file\":\"src/engine.rs\",\"before_range\":[2,2],\"after_range\":[2,2],\"before_anchor_hashes\":[\"winnow:0000000000000103\"],\"after_anchor_hashes\":[\"{1}\"],\"similarity\":0.95}}\n"
        ),
        uuid, span_anchor
    );

    let _ = run_json(repo, &["record", "--stdin"], Some(&tape_base));
    let _ = run_json(repo, &["record", "--stdin"], Some(&tape_worker));
    let second = run_json(repo, &["record", "--stdin"], Some(&tape_worker));
    assert_eq!(second["already_exists"], true);
    assert_eq!(second["already_indexed"], true);

    let _ = run_json(repo, &["record", "--stdin"], Some(&tape_restart));
    let explain = run_json(repo, &["explain", "src/engine.rs:2-2"], None);
    assert_eq!(explain["sessions"].as_array().expect("sessions").len(), 3);
    assert!(
        explain["dispatch_lineage"]
            .as_array()
            .expect("dispatch lineage")
            .len()
            >= 1
    );
}
