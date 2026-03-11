use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use engram::tape::adapter::{AdapterId, convert_with_adapter, discover_sessions_with_adapter};
use engram::tape::event::parse_jsonl_events;
use sha2::{Digest, Sha256};

fn canonical_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("canonical path")
}

fn canonical_text(path: &Path) -> String {
    canonical_path(path).to_string_lossy().into_owned()
}

fn repo_dash_key(path: &Path) -> String {
    canonical_text(path).replace('/', "-")
}

fn repo_hash(path: &Path) -> String {
    let canonical = canonical_text(path);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn write_fixture(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdirs");
    fs::write(path, contents).expect("write fixture");
}

fn assert_importable_with_adapter(adapter: AdapterId, transcript_path: &Path) {
    let raw = fs::read_to_string(transcript_path).expect("read transcript");
    let normalized = convert_with_adapter(adapter, &raw).expect("adapter conversion");
    let events = parse_jsonl_events(&normalized).expect("normalized parse");
    assert!(
        !events.is_empty(),
        "expected parsed events for adapter={adapter:?} path={}",
        transcript_path.display()
    );
}

fn codex_session_for_cwd(cwd: &Path) -> String {
    format!(
        concat!(
            "{{\"timestamp\":\"2026-02-22T00:00:00Z\",\"type\":\"session_meta\",",
            "\"payload\":{{\"cwd\":\"{}\",\"git\":{{\"commit_hash\":\"abc123\"}}}}}}\n",
            "{{\"timestamp\":\"2026-02-22T00:00:01Z\",\"type\":\"response_item\",",
            "\"payload\":{{\"type\":\"function_call\",\"name\":\"exec_command\",",
            "\"call_id\":\"call_1\",\"arguments\":\"{{\\\"cmd\\\":\\\"echo hi\\\"}}\"}}}}\n",
            "{{\"timestamp\":\"2026-02-22T00:00:02Z\",\"type\":\"response_item\",",
            "\"payload\":{{\"type\":\"function_call_output\",\"call_id\":\"call_1\",",
            "\"output\":\"Process exited with code 0\\nOutput:\\nhi\"}}}}\n"
        ),
        cwd.to_string_lossy()
    )
}

#[test]
fn real_layout_claude_discovery_and_import_validation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let wrong_repo = temp.path().join("wrong-repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(&wrong_repo).expect("wrong repo");

    let project_key = repo_dash_key(&repo);
    let transcript = home
        .join(".claude/projects")
        .join(project_key)
        .join("main.jsonl");
    let noise = home
        .join(".claude/projects")
        .join("other-project")
        .join("noise.jsonl");
    write_fixture(
        &transcript,
        include_str!("fixtures/claude_adapter_input.jsonl"),
    );
    write_fixture(&noise, include_str!("fixtures/claude_adapter_input.jsonl"));

    let discovered = discover_sessions_with_adapter(AdapterId::ClaudeCode, &repo, &home);
    assert_eq!(discovered, vec![transcript.clone()]);
    let negative = discover_sessions_with_adapter(AdapterId::ClaudeCode, &wrong_repo, &home);
    assert!(negative.is_empty(), "negative={negative:?}");
    assert_importable_with_adapter(AdapterId::ClaudeCode, &transcript);
}

#[test]
fn real_layout_codex_discovery_and_import_validation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let wrong_repo = temp.path().join("wrong-repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(&wrong_repo).expect("wrong repo");

    let sessions = home.join(".codex/sessions/2026/03/10");
    let transcript = sessions.join("session.jsonl");
    let noise = sessions.join("noise.jsonl");
    write_fixture(&transcript, &codex_session_for_cwd(&repo));
    write_fixture(&noise, &codex_session_for_cwd(&wrong_repo));

    let discovered = discover_sessions_with_adapter(AdapterId::CodexCli, &repo, &home);
    assert_eq!(discovered, vec![transcript.clone()]);
    let negative = discover_sessions_with_adapter(AdapterId::CodexCli, &wrong_repo, &home);
    assert_eq!(negative, vec![noise.clone()]);
    assert_importable_with_adapter(AdapterId::CodexCli, &transcript);
}

#[test]
fn real_layout_gemini_discovery_and_import_validation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let wrong_repo = temp.path().join("wrong-repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(&wrong_repo).expect("wrong repo");

    let bucket = home.join(".gemini/tmp").join(repo_hash(&repo));
    let chat = bucket.join("chats/session-2026.json");
    let logs = bucket.join("logs.json");
    let other = home
        .join(".gemini/tmp")
        .join(repo_hash(&wrong_repo))
        .join("chats/session-ignore.json");
    write_fixture(
        &chat,
        include_str!("fixtures/gemini/session_with_tools.json"),
    );
    write_fixture(&logs, include_str!("fixtures/gemini/logs.json"));
    write_fixture(
        &other,
        include_str!("fixtures/gemini/session_with_tools.json"),
    );

    let discovered = discover_sessions_with_adapter(AdapterId::GeminiCli, &repo, &home);
    assert_eq!(discovered, vec![chat.clone(), logs.clone()]);
    let negative = discover_sessions_with_adapter(AdapterId::GeminiCli, &wrong_repo, &home);
    assert_eq!(negative, vec![other.clone()]);
    assert_importable_with_adapter(AdapterId::GeminiCli, &chat);
    assert_importable_with_adapter(AdapterId::GeminiCli, &logs);
}

#[test]
fn real_layout_openclaw_discovery_and_import_validation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let wrong_repo = temp.path().join("wrong-repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(&wrong_repo).expect("wrong repo");

    let transcript = home
        .join(".openclaw/sessions")
        .join(repo_dash_key(&repo))
        .join("session.jsonl");
    let noise = home
        .join(".openclaw/sessions")
        .join(repo_dash_key(&wrong_repo))
        .join("noise.jsonl");
    write_fixture(
        &transcript,
        include_str!("fixtures/openclaw/session_log.jsonl"),
    );
    write_fixture(&noise, include_str!("fixtures/openclaw/session_log.jsonl"));

    let discovered = discover_sessions_with_adapter(AdapterId::OpenClaw, &repo, &home);
    assert_eq!(discovered, vec![transcript.clone()]);
    let negative = discover_sessions_with_adapter(AdapterId::OpenClaw, &wrong_repo, &home);
    assert_eq!(negative, vec![noise.clone()]);
    assert_importable_with_adapter(AdapterId::OpenClaw, &transcript);
}

#[test]
fn real_layout_opencode_discovery_and_import_validation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let wrong_repo = temp.path().join("wrong-repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(&wrong_repo).expect("wrong repo");

    let init_repo = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("init")
        .status()
        .expect("git init repo");
    assert!(init_repo.success(), "git init repo failed");
    let init_wrong = Command::new("git")
        .arg("-C")
        .arg(&wrong_repo)
        .arg("init")
        .status()
        .expect("git init wrong repo");
    assert!(init_wrong.success(), "git init wrong repo failed");

    let project_id = "project-real-validation";
    fs::write(repo.join(".git/opencode"), format!("{project_id}\n")).expect("opencode cache");
    fs::write(wrong_repo.join(".git/opencode"), "wrong-project\n").expect("wrong cache");

    let transcript = home
        .join(".local/share/opencode/project")
        .join(project_id)
        .join("storage/session/info/info.json");
    let noise = home
        .join(".local/share/opencode/project/wrong-project")
        .join("storage/session/info/noise.json");
    write_fixture(
        &transcript,
        include_str!("fixtures/opencode/session_export.json"),
    );
    write_fixture(
        &noise,
        include_str!("fixtures/opencode/session_export.json"),
    );

    let discovered = discover_sessions_with_adapter(AdapterId::OpenCode, &repo, &home);
    assert_eq!(discovered, vec![transcript.clone()]);
    let negative = discover_sessions_with_adapter(AdapterId::OpenCode, &wrong_repo, &home);
    assert_eq!(negative, vec![noise.clone()]);
    assert_importable_with_adapter(AdapterId::OpenCode, &transcript);
}

#[test]
fn real_layout_cursor_discovery_and_import_validation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let wrong_repo = temp.path().join("wrong-repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(&wrong_repo).expect("wrong repo");

    let workspace_root = home.join("Library/Application Support/Cursor/User/workspaceStorage");
    let matching_workspace = workspace_root.join("abc");
    let wrong_workspace = workspace_root.join("def");
    fs::create_dir_all(&matching_workspace).expect("matching workspace dir");
    fs::create_dir_all(&wrong_workspace).expect("wrong workspace dir");

    let transcript = matching_workspace.join("state.vscdb");
    let noise = wrong_workspace.join("state.vscdb");
    write_fixture(
        &matching_workspace.join("workspace.json"),
        format!("{{\"folder\":\"{}\"}}\n", canonical_text(&repo)).as_str(),
    );
    write_fixture(
        &wrong_workspace.join("workspace.json"),
        format!("{{\"folder\":\"{}\"}}\n", canonical_text(&wrong_repo)).as_str(),
    );
    write_fixture(
        &transcript,
        include_str!("fixtures/cursor/supported_paths.jsonl"),
    );
    write_fixture(
        &noise,
        include_str!("fixtures/cursor/supported_paths.jsonl"),
    );

    let discovered = discover_sessions_with_adapter(AdapterId::Cursor, &repo, &home);
    assert_eq!(discovered, vec![transcript.clone()]);
    let negative = discover_sessions_with_adapter(AdapterId::Cursor, &wrong_repo, &home);
    assert_eq!(negative, vec![noise.clone()]);
    assert_importable_with_adapter(AdapterId::Cursor, &transcript);
}
