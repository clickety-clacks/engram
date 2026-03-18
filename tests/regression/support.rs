use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub fn run_cli(repo: &Path, args: &[&str], stdin: Option<&str>, home: &Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engram"));
    fs::create_dir_all(home).expect("home dir");
    cmd.current_dir(repo).args(args).env("HOME", home);
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

pub fn run_json(repo: &Path, args: &[&str], stdin: Option<&str>, home: &Path) -> Value {
    let output = run_cli(repo, args, stdin, home);
    assert!(
        output.status.success(),
        "command failed: args={args:?}\\nstdout={}\\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json stdout")
}

pub fn run_json_timed(
    repo: &Path,
    args: &[&str],
    stdin: Option<&str>,
    home: &Path,
) -> (Value, Duration) {
    let start = Instant::now();
    let out = run_json(repo, args, stdin, home);
    (out, start.elapsed())
}
