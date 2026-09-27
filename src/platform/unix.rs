use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};

use crate::access::transport::launch_spec;
use crate::config::TopologyPeer;
use crate::CliError;

pub(super) fn home_dir() -> Result<PathBuf, CliError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| CliError::new("home_error", "HOME environment variable is not set"))
}

pub(super) fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::RootDir | Component::Normal(_) => out.push(component.as_os_str()),
            Component::Prefix(_) => unreachable!("Unix paths do not have prefixes"),
        }
    }
    out
}

pub(super) fn canonicalize_or_normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

pub(super) fn repository_path_key(path: &Path) -> String {
    canonicalize_or_normalize(path)
        .to_string_lossy()
        .replace('/', "-")
}

pub(super) fn file_uri_path(decoded: &str) -> Option<PathBuf> {
    decoded.starts_with('/').then(|| PathBuf::from(decoded))
}

pub(super) fn open_read_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    options.open(path)
}

pub(super) fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

pub(super) fn sync_parent_dir(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

pub(super) fn spawn_peer(peer: &TopologyPeer) -> io::Result<Child> {
    let spec = launch_spec(peer)?;
    let mut command = Command::new(spec.program);
    command
        .args(spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command.spawn()
}

pub(super) fn exit_status_description(status: ExitStatus) -> String {
    if let Some(signal) = status.signal() {
        return format!("terminated by signal {signal}");
    }
    status
        .code()
        .map(|code| format!("exited with code {code}"))
        .unwrap_or_else(|| "terminated without an exit code".into())
}

pub(super) fn terminate_peer_process_group(child: &mut Child) {
    let process_group = child.id() as libc::pid_t;
    if process_group > 0 {
        // Peer shells may leave descendants holding the piped stderr/stdout.
        // Kill the isolated group before joining the pipe readers.
        let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    }
}

pub(super) fn ensure_peer_commands_supported() -> Result<(), CliError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nofollow_read_refuses_symbolic_link_tapes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("real.jsonl.zst");
        let link = temp.path().join("link.jsonl.zst");
        fs::write(&target, b"tape").expect("target");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let error = open_read_nofollow(&link).expect_err("refuse symlink");
        assert_eq!(error.kind(), io::ErrorKind::FilesystemLoop);
    }

    #[test]
    fn atomic_replace_replaces_existing_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let destination = temp.path().join("index.sqlite");
        let replacement = temp.path().join("index.sqlite.tmp");
        fs::write(&destination, b"old").expect("destination");
        fs::write(&replacement, b"new").expect("replacement");

        atomic_replace(&replacement, &destination).expect("atomic replacement");
        sync_parent_dir(temp.path()).expect("parent sync");

        assert_eq!(fs::read(destination).expect("read replacement"), b"new");
        assert!(!replacement.exists());
    }
}
