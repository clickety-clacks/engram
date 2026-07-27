use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

use serde_json::{Value, json};

const DEPRECATION_LINE: &str = "deprecation: engram gc is deprecated and permanently non-destructive; tapes are immutable and the derived index is maintained by explicit rebuild/validate/swap/retire (see `engram gc --help`)";

fn run_cli(repo: &Path, args: &[&str]) -> Output {
    let isolated_home = repo.join(".home");
    fs::create_dir_all(&isolated_home).expect("home dir");
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(repo)
        .env("HOME", isolated_home)
        .args(args)
        .output()
        .expect("command runs")
}

fn seed_store(repo: &Path) -> PathBuf {
    let init = run_cli(repo, &["init"]);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let tape_path = repo
        .join(".engram/tapes")
        .join(format!("{}.jsonl.zst", "0123456789abcdef".repeat(4)));
    fs::write(&tape_path, b"immutable tape bytes").expect("seed tape");
    tape_path
}

#[derive(Debug, PartialEq, Eq)]
struct FileSnapshot {
    path: PathBuf,
    modified: SystemTime,
    contents: Vec<u8>,
}

fn snapshot(paths: &[PathBuf]) -> Vec<FileSnapshot> {
    paths
        .iter()
        .map(|path| {
            let metadata = fs::metadata(path).expect("file metadata");
            FileSnapshot {
                path: path.clone(),
                modified: metadata.modified().expect("modified time"),
                contents: fs::read(path).expect("file contents"),
            }
        })
        .collect()
}

#[test]
fn gc_reports_deprecated_json_and_stderr_contract() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let _tape_path = seed_store(repo);

    let output = run_cli(repo, &["gc"]);
    assert!(
        output.status.success(),
        "gc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: Value = serde_json::from_slice(&output.stdout).expect("stdout is only JSON");
    assert_eq!(
        payload,
        json!({
            "status": "ok",
            "deprecated": true,
            "deleted_tape_ids": [],
            "deleted_count": 0,
            "kept_count": 1,
        })
    );

    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(!stdout.contains("config:"));
    assert!(!stdout.contains("db:"));
    assert!(!stdout.contains("deprecation:"));

    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(stderr.lines().any(|line| line.starts_with("config: ")));
    assert!(stderr.lines().any(|line| line.starts_with("db: ")));
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("deprecation:"))
            .collect::<Vec<_>>(),
        vec![DEPRECATION_LINE]
    );
}

#[test]
fn gc_does_not_modify_any_store_artifact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let tape_path = seed_store(repo);
    let local_store = repo.join(".engram");
    let index_store = repo.join(".home/.engram");
    let paths = [
        tape_path,
        index_store.join("index.sqlite"),
        index_store.join("index.sqlite.pre-test"),
        local_store.join("objects/blob"),
        local_store.join("cursors/source.json"),
    ];
    for (index, path) in paths.iter().enumerate().skip(1) {
        fs::write(path, format!("artifact {index}")).expect("seed artifact");
    }
    let before = snapshot(&paths);

    let output = run_cli(repo, &["gc"]);
    assert!(output.status.success(), "gc succeeds");

    assert_eq!(snapshot(&paths), before);
}

#[test]
fn gc_help_documents_lifecycle_without_advertising_gc() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();

    let gc_help = run_cli(repo, &["gc", "--help"]);
    assert!(gc_help.status.success(), "gc help succeeds");
    let gc_help = String::from_utf8(gc_help.stdout).expect("utf-8 gc help");
    for phrase in [
        "Deprecated",
        "deletes nothing",
        "rebuild",
        "validate",
        "swap",
        "retire",
        "docs/index-lifecycle.md",
    ] {
        assert!(gc_help.contains(phrase), "gc help is missing {phrase:?}");
    }

    let top_help = run_cli(repo, &["--help"]);
    assert!(top_help.status.success(), "top-level help succeeds");
    let top_help = String::from_utf8(top_help.stdout).expect("utf-8 top-level help");
    assert!(
        !top_help
            .lines()
            .any(|line| line.trim_start().starts_with("gc")),
        "top-level help must not advertise gc"
    );
}
