use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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
fn clawline_style_tape_yields_agent_useful_peek_content() {
    let fixture = fs::read_to_string("tests/fixtures/clawline_sample.jsonl").expect("fixture");
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    let _ = run_json(repo, &["init"], None);
    let _ = run_json(repo, &["record", "--stdin"], Some(&fixture));

    let explain = run_json(
        repo,
        &["explain", "winnow:0000000000000301", "--anchor"],
        None,
    );
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1);

    let session_id = sessions[0]["session_id"]
        .as_str()
        .expect("session_id");
    let window_start = sessions[0]["window_start"]
        .as_u64()
        .expect("window_start") as usize;
    let window_end = sessions[0]["window_end"].as_u64().expect("window_end") as usize;
    let window_lines = window_end.saturating_sub(window_start).saturating_add(1);

    let peek = run_json(
        repo,
        &[
            "peek",
            session_id,
            "--start",
            &window_start.to_string(),
            "--lines",
            &window_lines.to_string(),
        ],
        None,
    );
    let content = peek["session"]["content"].as_array().expect("content");
    assert!(!content.is_empty(), "expected peek content around touch events");

    let mut saw_contextual_event = false;
    for line in content {
        let text = line["text"].as_str().unwrap_or("");
        let Ok(event) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let kind = event["k"].as_str().unwrap_or("");
        if kind == "msg.in" || kind == "msg.out" || kind == "tool.call" || kind == "tool.result" {
            saw_contextual_event = true;
            break;
        }
    }

    assert!(
        saw_contextual_event,
        "expected explain windows to include conversational/tool context"
    );
}
