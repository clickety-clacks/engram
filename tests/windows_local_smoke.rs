use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};

fn run_cli(repo: &Path, home: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    command.current_dir(repo).args(args);
    if cfg!(windows) {
        command.env("USERPROFILE", home).env_remove("HOME");
    } else {
        command.env("HOME", home);
    }
    command.output().expect("compiled Engram binary runs")
}

fn run_json(repo: &Path, home: &Path, args: &[&str]) -> Value {
    let output = run_cli(repo, home, args);
    assert!(
        output.status.success(),
        "Engram command should succeed: args={args:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Engram emits JSON")
}

#[cfg(windows)]
fn assert_unsupported_peer_command(repo: &Path, home: &Path, args: &[&str]) {
    let output = run_cli(repo, home, args);
    assert!(
        !output.status.success(),
        "peer command must be rejected: {args:?}"
    );
    let error: Value = serde_json::from_slice(&output.stderr).expect("structured CLI error");
    assert_eq!(error["error"]["code"], "unsupported_platform");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unsupported on Windows")),
        "the unsupported reason should be explicit: {error}"
    );
}

#[test]
fn local_ingest_explain_grep_peek_and_show_work_with_the_platform_home() {
    let temp = tempfile::tempdir().expect("temporary smoke root");
    let home = temp.path().join("user-home");
    let repo = home.join("repo");
    let source_dir = repo.join("src");
    fs::create_dir_all(&source_dir).expect("create isolated local repo");

    let initialized = run_json(&repo, &home, &["init"]);
    assert_eq!(initialized["status"], "ok");

    let unchanged = (1..=20)
        .map(|line| format!("    let stable_{line:02} = {line};\n"))
        .collect::<String>();
    let before_changed = (21..=30)
        .map(|line| format!("    let legacy_{line:02} = {line};\n"))
        .collect::<String>();
    let after_changed = (21..=30)
        .map(|line| {
            if line == 25 {
                "    let marker = \"portable Windows smoke marker\";\n".to_string()
            } else {
                format!("    let current_{line:02} = {line};\n")
            }
        })
        .collect::<String>();
    let before = format!("pub fn smoke() -> usize {{\n{unchanged}{before_changed}    1\n}}\n");
    let after = format!("pub fn smoke() -> usize {{\n{unchanged}{after_changed}    1\n}}\n");
    let source_path = source_dir.join("smoke.rs");
    fs::write(&source_path, &after).expect("write current local source");

    let session = "windows-local-smoke";
    let tool_id = "smoke-edit-1";
    let rows = [
        json!({
            "type":"assistant",
            "session_id":session,
            "timestamp":"2026-09-26T12:00:00Z",
            "message":{
                "model":"claude-fable-5",
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":tool_id,
                    "name":"Edit",
                    "input":{
                        "file_path":"src/smoke.rs",
                        "old_string":before,
                        "new_string":after
                    }
                }]
            }
        }),
        json!({
            "type":"user",
            "session_id":session,
            "timestamp":"2026-09-26T12:00:01Z",
            "message":{
                "role":"user",
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":tool_id,
                    "content":"The file was updated successfully."
                }]
            }
        }),
    ];
    let transcript_path = repo.join("smoke.jsonl");
    let transcript = rows
        .iter()
        .map(|row| serde_json::to_string(row).expect("serialize transcript row"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&transcript_path, transcript).expect("write isolated Claude transcript");
    let transcript_arg = transcript_path.to_string_lossy().into_owned();
    let ingest = run_json(&repo, &home, &["ingest", &transcript_arg]);
    assert_eq!(ingest["imported_tapes"], 1);

    let tapes = run_json(&repo, &home, &["tapes"]);
    let tape_id = tapes["tapes"][0]["tape_id"]
        .as_str()
        .expect("locally ingested tape ID")
        .to_string();

    let explain = run_json(&repo, &home, &["explain", "src/smoke.rs"]);
    assert!(
        explain["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions
                .iter()
                .any(|session| session["session_id"] == tape_id)),
        "explain should find the locally ingested edit"
    );

    let grep = run_json(&repo, &home, &["grep", "portable Windows smoke marker"]);
    assert!(
        grep["sessions"].as_array().is_some_and(|sessions| sessions
            .iter()
            .any(|session| session["session_id"] == tape_id)),
        "grep should find text in the local tape"
    );

    let peek = run_json(&repo, &home, &["peek", &tape_id]);
    assert_eq!(peek["session"]["session_id"], tape_id);
    assert!(!peek["session"]["content"].as_array().unwrap().is_empty());

    let show = run_json(&repo, &home, &["show", &tape_id]);
    assert_eq!(show["tape_id"], tape_id);
    assert!(show["event_count"].as_u64().unwrap() >= 2);

    #[cfg(windows)]
    {
        assert_unsupported_peer_command(
            &repo,
            &home,
            &["explain", "src/smoke.rs", "--peers", "all"],
        );
        assert_unsupported_peer_command(&repo, &home, &["grep", "marker", "--peers", "all"]);
        assert_unsupported_peer_command(
            &repo,
            &home,
            &["peek", &tape_id, "--store", "peer/default"],
        );
        assert_unsupported_peer_command(&repo, &home, &["show", &tape_id, "--peers", "all"]);
        assert_unsupported_peer_command(&repo, &home, &["topology", "status"]);
        assert_unsupported_peer_command(&repo, &home, &["peer-serve", "--stdio"]);
    }
}
